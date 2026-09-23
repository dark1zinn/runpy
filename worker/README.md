# runpyrs

Python worker SDK for [Runpy](https://github.com/dark1zinn/runpy) — write Python workers that are spawned and managed by the Rust-side `Manager`.

## Installation

`uv` is required. Until `runpyrs` is published, install it from the repository
subdirectory:

```bash
uv add "git+https://github.com/dark1zinn/runpy#subdirectory=worker"
```

## Quick Start

```python
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

## License

Apache-2.0

> Brought to you by [@dark1zinn](https://github.com/dark1zinn) with ❤️
