"""Command-line bootstrap used by scripts launched from the Rust Manager."""

import os
import sys

from typing import Dict, Type
from .worker import Worker

# ── Helper ──────────────────────────────────────────────────────────────


def _parse_extra_args(args: list[str]) -> Dict[str, str]:
    """Parse ``--key=value`` arguments, ignoring every other token.

    Later occurrences of the same key replace earlier values. Values may
    contain additional ``=`` characters.
    """
    extra: Dict[str, str] = {}
    for arg in args:
        if arg.startswith("--") and "=" in arg:
            key, value = arg[2:].split("=", 1)
            extra[key] = value
    return extra


def _fail(message: str) -> None:
    """Write a diagnostic to stderr when possible, then exit with status 1.

    A closed or broken stderr must not hide the failure status, so write
    errors are ignored. A failed write leaves the text buffered in the
    stream, and the interpreter retries that flush at shutdown, where a second
    failure turns the exit status into 120; the broken stream is therefore
    replaced before exiting.
    """
    try:
        print(message, file=sys.stderr, flush=True)
    except (OSError, ValueError, AttributeError):
        try:
            sys.stderr = open(os.devnull, "w")
        except OSError:
            sys.stderr = None
    sys.exit(1)


def RunScript(worker_class: Type[Worker]) -> None:
    """Instantiate a :class:`Worker` subclass and run its receive loop.

    The Rust Manager supplies the Unix socket path in ``sys.argv[1]`` and the
    required worker identity in ``sys.argv[2]``. Remaining ``--key=value``
    tokens populate :attr:`Worker.extra`.

    Returns normally (exit status 0) after ``terminate`` or when the Manager
    closes the connection. Missing arguments, a class that does not inherit
    :class:`Worker`, a construction or connection failure, a protocol
    violation, or a socket failure writes one diagnostic to stderr and exits
    with status 1. ``SystemExit`` and ``KeyboardInterrupt`` raised by worker
    code propagate unchanged.

    Example:
        .. code-block:: python

            from runpyrs import RunScript, Worker

            class MyWorker(Worker):
                def execute(self, data: dict) -> dict:
                    return {"result": data}

            if __name__ == "__main__":
                RunScript(MyWorker)
    """
    if len(sys.argv) < 3:
        _fail("Error: Socket path and worker ID arguments required")

    socket_path = sys.argv[1]
    worker_name = sys.argv[2]
    extra_args = _parse_extra_args(sys.argv[3:])

    if not isinstance(worker_class, type) or not issubclass(worker_class, Worker):
        name = getattr(worker_class, "__name__", repr(worker_class))
        _fail(f"Configuration error: {name} must inherit from Worker")

    try:
        worker = worker_class(socket_path, worker_name, extra_args)
    except Exception as error:
        _fail(f"Worker initialization failed: {type(error).__name__}: {error}")

    try:
        worker.run()
    except Exception as error:
        _fail(f"Protocol violation: {type(error).__name__}: {error}")
