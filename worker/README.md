# runpyrs

Python worker SDK for [Runpy](https://github.com/dark1zinn/runpy) — write Python workers that are spawned and managed by the Rust-side `Manager`.

## Managed script setup

`uv` is required by the Rust manager in development and production. Initialize
each worker as a PEP 723 script and declare `runpyrs` in that script:

```bash
uv init --script my_worker.py --python 3.10
uv add --script my_worker.py \
  "runpyrs @ git+https://github.com/dark1zinn/runpy#subdirectory=worker"
uv lock --script my_worker.py
```

The lockfile is optional at runtime but should be committed for reproducible
deployments.

## Quick Start

```python
# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "runpyrs @ git+https://github.com/dark1zinn/runpy#subdirectory=worker",
# ]
# ///

from runpyrs import Envelope, Worker, RunScript


class MyWorker(Worker):
    def execute(self, data: dict) -> dict:
        return {"status": "ok", "result": data}

    def handle_envelope(self, envelope: Envelope) -> None:
        self.send(
            {"accepted": True},
            meta={"correlation_id": envelope["meta"].get("correlation_id")},
        )


if __name__ == "__main__":
    RunScript(MyWorker)
```

The Rust manager supplies the Unix socket path and worker ID. `RunScript`
connects the worker and stamps both values into Runpy-reserved metadata.

The Rust manager starts this file with `uv run --no-project --script`.
`uv` selects or downloads a Python satisfying `requires-python`, resolves the
inline dependencies into an isolated cached environment, and reuses
`my_worker.py.lock` when present. Runpy passes `--locked` for an adjacent lock,
so stale locks fail instead of changing during launch; scripts without locks
still resolve normally. Runpy does not create a project `.venv`, run `uv sync`,
mutate locks, or inject the SDK.

Use `Manager::new("path/to/scripts")` when `uv` is on `PATH`, or
`Manager::with_uv_path("path/to/scripts", "/packaged/path/to/uv")` for an
explicit executable.

For an additional reproducibility boundary, PEP 723 metadata accepts uv's
RFC 3339 upload cutoff:

```python
# [tool.uv]
# exclude-newer = "2025-01-01T00:00:00Z"
```

## Envelope

Every message has exactly two object fields:

```json
{
    "meta": {
        "x_wid": "worker-id",
        "x_spath": "/tmp/runpy/rp_worker.sock",
        "some_custom_meta": 42
    },
    "data": {
        "some": "data"
    }
}
```

Application code owns `data` and metadata keys that do not begin with `x_`.
Runpy reserves `x_wid`, `x_spath`, and `x_op`. The internal operations are
`ready`, `execute`, `retry`, `terminate`, `done`, `error`, and `log`.

## API

| Symbol | Description |
| --- | --- |
| `Worker` | Base class for managed Python workers. |
| `RunScript` | CLI bootstrap used by scripts launched from Rust. |
| `Worker.send(data, meta=...)` | Send a custom envelope with no internal operation. |
| `Worker.log(data, level=..., meta=...)` | Send a structured `log` envelope. |
| `Worker.execute(data)` | Override to process an `execute` envelope. |
| `Worker.handle_envelope(envelope)` | Override for custom envelopes with no `x_op`. |
| `create_envelope(data, meta=...)` | Build a custom typed envelope and reject reserved keys. |

`Worker.log(...)` normally remains a structured `x_op="log"` envelope. If its
Unix-socket send raises `OSError`, the SDK prints one flushed stdout fallback:

```text
[runpy-log-fallback][level=<level>] <data repr>
```

The original `OSError` is re-raised, so a disconnected worker does not appear
healthy. The Rust Manager captures stdout and adds the trusted worker ID; the
Python fallback does not provide its own identity. Delivery is best effort and
may be duplicated when a socket failure is reported after the Manager already
received the complete envelope. Serialization and metadata errors do not use
the fallback.

## License

Apache-2.0

> Brought to you by [@dark1zinn](https://github.com/dark1zinn) with ❤️
