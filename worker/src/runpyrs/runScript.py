import sys

from typing import Type
from .worker import Worker

# ── Helper ──────────────────────────────────────────────────────────────

def RunScript(worker_class: Type[Worker]):
    """Instantiate and run a Worker subclass.

    Reads the socket path from ``sys.argv[1]``.
    """
    try:
        if len(sys.argv) < 2:
            print("Error: Socket path argument required")
            sys.exit(1)

        socket_path = sys.argv[1]

        if not issubclass(worker_class, Worker):
            raise TypeError(f"{worker_class.__name__} must inherit from Worker")

        worker = worker_class(socket_path)
        worker.run()

    except TypeError as e:
        print(f"Configuration error: {e}")
        sys.exit(1)
    except Exception as e:
        print(f"Worker initialization failed: {e}")
        sys.exit(1)
