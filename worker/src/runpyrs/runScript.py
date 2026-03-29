import sys

from typing import Dict, Optional, Type
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

    Reads the socket path from ``sys.argv[1]``, optionally the worker
    name from ``sys.argv[2]``, and extra --key=value arguments from
    ``sys.argv[3:]`` (all passed by Rust manager at spawn time).
    """
    try:
        if len(sys.argv) < 2:
            print("Error: Socket path argument required")
            sys.exit(1)

        socket_path = sys.argv[1]
        worker_name: Optional[str] = sys.argv[2] if len(sys.argv) > 2 else None
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
