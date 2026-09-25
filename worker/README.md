# runpyrs

`runpyrs` is the Python worker SDK for
[Runpy](https://github.com/dark1zinn/runpy). A Rust `Manager` starts each
self-contained Python script through `uv`; `RunScript` connects the script to
its Unix socket and drives a `Worker` subclass.

The package is currently installed from Git, not PyPI. See the
[installation guide](../docs/installation.md) for both packages and the
[project overview](../docs/overview.md) for architecture and protocol details.

## Managed script setup

Create a PEP 723 script and declare `runpyrs` as a dependency:

```bash
uv init --script my_worker.py --python 3.10
uv add --script my_worker.py \
  "runpyrs @ git+https://github.com/dark1zinn/runpy#subdirectory=worker"

# Optional but recommended for reproducible deployments
uv lock --script my_worker.py
```

The script metadata supplies its complete Python and dependency requirements.
The Rust Manager starts it with `uv run --no-project --script` and adds
`--locked` when `my_worker.py.lock` exists. Runpy does not create a project
virtual environment, run `uv sync`, inject `runpyrs`, or update the lockfile.

## Quick start

```python
# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "runpyrs @ git+https://github.com/dark1zinn/runpy#subdirectory=worker",
# ]
# ///

from runpyrs import Envelope, RunScript, Worker


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

The Manager supplies the socket path and trusted worker ID as positional
arguments. Additional `--key=value` arguments become `Worker.extra`.
`RunScript` constructs the class, sends `ready`, and runs the receive loop.

## Execution model

- `execute(data)` handles `execute`; return a dictionary for `done.data` or
  `None` for an empty result.
- An exception or non-dictionary result becomes an `error` envelope. It does
  not automatically terminate the worker.
- `retry` repeats the last execute payload. Retry before execute returns an
  error envelope.
- `handle_envelope(envelope)` receives application messages without `x_op`.
- `send(data, meta=...)` sends an application envelope.
- `log(data, level=..., meta=...)` sends a structured `log` envelope.
- `run()` closes the connection on malformed frames, invalid reserved
  metadata, or a wrong-direction operation.

## Envelope model

Every message is exactly two objects:

```json
{
  "meta": {
    "x_wid": "manager-generated-worker-id",
    "x_spath": "/tmp/runpy/rp_worker.sock",
    "correlation_id": 42
  },
  "data": {
    "task": "parse"
  }
}
```

Applications own `data` and metadata keys that do not begin with `x_`. Runpy
reserves the complete `x_` namespace:

| Key | Meaning |
| --- | --- |
| `x_wid` | Trusted worker identity stamped by Runpy. |
| `x_spath` | Trusted socket path stamped by Runpy. |
| `x_op` | Optional lifecycle operation. |

The reserved operations are `ready`, `execute`, `retry`, `terminate`, `done`,
`error`, and `log`. `create_envelope` copies its inputs and rejects
application-provided `x_` metadata.

## Public API

| Symbol | Contract |
| --- | --- |
| `Worker` | Base class for a managed worker connection and receive loop. |
| `RunScript` | CLI bootstrap that validates a `Worker` subclass and consumes Manager arguments. |
| `Worker.send(data, meta=...)` | Send a custom envelope with no internal operation. |
| `Worker.log(data, level=..., meta=...)` | Send a structured `log` envelope with transport-failure fallback. |
| `Worker.execute(data)` | Override for managed execution; return `dict` or `None`. |
| `Worker.handle_envelope(envelope)` | Override for custom envelopes with no `x_op`. |
| `Worker.run()` | Receive and dispatch until termination or protocol failure. |
| `create_envelope(data, meta=...)` | Copy a custom envelope and reject reserved metadata. |
| `Envelope`, `Meta`, `Data` | Typed dictionary aliases for the wire model. |
| `RunpyOperation` | Literal type for all reserved operation strings. |
| `ExecutePayload`, `ExecuteResult` | Execute input and optional dictionary result aliases. |

The package includes `py.typed`, so type checkers can consume these annotations.

## Structured log fallback

`Worker.log` normally sends `x_op="log"` over the socket. If that operation
raises `OSError`, the SDK makes one best-effort flushed stdout write:

```text
[runpy-log-fallback][level=<level>] <data repr>
```

The original socket error is always re-raised. A fallback `OSError` or the
`ValueError` raised by closed stdout is suppressed so it cannot replace the
transport failure. Serialization, reserved-metadata, and other programmer
errors do not use the fallback.

The fallback deliberately contains no Python-provided worker identity. The
Rust Manager captures stdout and adds trusted attribution. Delivery is
at-least-once/best-effort: if a send fails after the Manager accepted a complete
frame, it may observe both the structured log and fallback line.

## More documentation

- [Repository README](../README.md)
- [Installation and worker setup](../docs/installation.md)
- [Architecture and protocol overview](../docs/overview.md)
- [End-to-end playground](../examples/playground/)

## License

Apache-2.0 — see [LICENSE](../LICENSE).
