import sys

from typing import Dict, Type
from .worker import Worker

# ── Helper ──────────────────────────────────────────────────────────────


def _parse_extra_args(args: list[str]) -> Dict[str, str]:
    """Parse --key=value arguments into a dict."""
    extra: Dict[str, str] = {}
    for arg in args:
        if arg.startswith("--") and "=" in arg:
            key, value = arg[2:].split("=", 1)
            extra[key] = value
    return extra


def RunScript(worker_class: Type[Worker]):
    """Instantiate and run a Worker subclass.

    Reads the socket path from ``sys.argv[1]`` and the required worker ID from
    ``sys.argv[2]``. Remaining ``--key=value`` arguments are exposed as
    ``Worker.extra``.
    """
    try:
        if len(sys.argv) < 3:
            print("Error: Socket path and worker ID arguments required")
            sys.exit(1)

        socket_path = sys.argv[1]
        worker_name = sys.argv[2]
        extra_args = _parse_extra_args(sys.argv[3:]) if len(sys.argv) > 3 else {}

        if not issubclass(worker_class, Worker):
            raise TypeError(f"{worker_class.__name__} must inherit from Worker")

        worker = worker_class(socket_path, worker_name, extra_args)
        worker.run()

    except TypeError as e:
        print(f"Configuration error: {e}")
        sys.exit(1)
    except Exception as e:
        print(f"Worker initialization failed: {e}")
        sys.exit(1)
