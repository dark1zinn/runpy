# runpyrs

Python worker SDK for [Runpy](https://github.com/dark1zinn/runpy) — write Python workers that are spawned and managed by the Rust-side `Manager`.

## Installation

```bash
pip install runpyrs
```

## Quick Start

```python
from runpyrs import Worker, RunScript

class MyWorker(Worker):
    def execute(self, payload: dict) -> dict:
        # Your business logic here
        return {"status": "ok", "result": payload}

if __name__ == "__main__":
    RunScript(MyWorker)
```

The Rust manager will launch your script, passing the Unix socket path as the first argument. `RunScript` handles the wiring automatically.

## How It Works

1. The Rust `Manager` spawns your Python script as a child process and provides a Unix socket path.
2. `RunScript` reads the socket path from `sys.argv`, instantiates your `Worker` subclass, and connects to the socket.
3. The worker sends a `READY` message and enters a loop waiting for commands (`EXECUTE`, `RETRY`, `TERMINATE`, `META`).
4. Override `execute()` to define your business logic. Return a dict and it is sent back as a `DONE` message.
5. Override `handle_request()` for any custom (non-internal) message types.

## API

| Symbol | Description |
| --- | --- |
| `Worker` | Base class — subclass it and override `execute()`. |
| `RunScript` | Helper that bootstraps a `Worker` subclass from the CLI. |
| `Worker.send(type, message, data)` | Send a typed message back to the Rust manager. |
| `Worker.execute(payload)` | *Override* — called on `EXECUTE` messages. |
| `Worker.handle_request(data)` | *Override* — called for non-internal message types. |

## License

Apache-2.0

> Brought to you by [@dark1zinn](https://github.com/dark1zinn) with ❤️
