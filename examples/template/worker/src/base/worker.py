import socket
import json
import os
import sys
import struct

from typing import Type


class Worker:
    """Base class for Python workers managed by Runpy.

    Subclass this and override `execute()` (required) and optionally
    `handle_request()` for custom message routing.

    Usage::

        class MyWorker(Worker):
            def execute(self, payload: dict) -> dict:
                return {"result": "ok"}

        if __name__ == "__main__":
            RunScript(MyWorker)
    """

    def __init__(self, sock):
        self.__ok = False
        self.__cycles = 0
        self.__exec_payload = None
        self.__sock_path = sock

        try:
            self.stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            self.stream.connect(self.__sock_path)
        except Exception as e:
            print(f"Failed to set up socket: {e}")
            sys.exit(1)

        self.__ok = True
        self.send("READY", "Worker is ready to receive requests")

    # ── Outbound messaging ──────────────────────────────────────────

    def __send_message(self, data: bytes):
        """Send a length-prefixed message"""
        try:
            size = struct.pack('<Q', len(data))
            self.stream.sendall(size + data)
        except Exception as e:
            print(f"Failed to send message: {e}")
            self.__ok = False

    def __recv_message(self) -> dict:
        """Receive a length-prefixed message"""
        try:
            size_data = self.stream.recv(8)
            if not size_data:
                return None

            size = struct.unpack('<Q', size_data)[0]

            data = b""
            while len(data) < size:
                chunk = self.stream.recv(size - len(data))
                if not chunk:
                    raise ConnectionError("Connection closed unexpectedly")
                data += chunk

            return json.loads(data.decode())
        except Exception as e:
            print(f"Error receiving message: {e}")
            return None

    def send(self, msg_type: str, message: str = None, data=None):
        """Send a typed message back to Rust's Control Plane."""
        try:
            if not msg_type:
                raise ValueError("Message type is required")
            payload = {
                "type": msg_type.upper(),
                "message": message or "",
                "data": data or {},
            }
            self.__send_message(json.dumps(payload).encode())
        except Exception as e:
            print(f"Failed to notify Rust: {e}")
            self.__ok = False
            sys.exit(1)

    # ── Overridable hooks ───────────────────────────────────────────

    def handle_request(self, request_data: dict) -> dict:
        """Handle incoming requests from Rust.

        Override this method in your worker subclass to implement custom
        message routing for types that are **not** handled internally
        (EXECUTE, TERMINATE, META, RETRY).

        The default implementation is a no-op so that subclasses are not
        forced to override it.
        """
        pass

    def execute(self, payload: dict) -> dict:
        """Main business logic.

        Called when an EXECUTE message is received.  Override this in your
        subclass and return a dict with the result.  The result is sent
        back to Rust as a DONE message.
        """
        return None

    # ── Internal request handling ───────────────────────────────────

    # Message types handled internally — custom `handle_request` will
    # NOT be called for these.
    _INTERNAL_TYPES = frozenset({"TERMINATE", "META", "EXECUTE", "RETRY"})

    def __handle_request(self, request_data: dict):
        """Internal dispatcher for protocol-level messages."""
        try:
            msg_type = request_data.get("type")

            if msg_type is None:
                self.send("DEBUG", "Received request with no type", {"request": request_data})
                return

            if msg_type == "TERMINATE":
                self.stream.close()
                sys.exit(0)

            if msg_type == "META":
                if request_data.get("data", {}).get("name"):
                    self.name = request_data["data"]["name"]
                self.send("DEBUG", "Received META data", request_data.get("data"))
                return

            if msg_type == "EXECUTE":
                self.__exec_payload = request_data.get("payload", request_data.get("data"))
                self.send("DEBUG", "Received EXECUTE request", {"payload": self.__exec_payload})
                try:
                    result = self.execute(self.__exec_payload)
                    if result is not None:
                        self.send("DONE", "Execution completed", result)
                    else:
                        self.send("DONE", "Execution completed with no result", {})
                except Exception as e:
                    self.send("ERROR", f"Execution error: {e}")
                return

            if msg_type == "RETRY":
                self.__cycles += 1
                self.send("DEBUG", f"Received RETRY request, re-executing (cycle: {self.__cycles})", {})
                try:
                    result = self.execute(self.__exec_payload)
                    if result is not None:
                        self.send("DONE", f"Execution completed on retry({self.__cycles})", result)
                    else:
                        self.send("DONE", f"Execution completed on retry({self.__cycles}) with no result", {})
                except Exception as e:
                    self.send("ERROR", f"Execution error on retry: {e}")
                return

            # Not an internal type — fall through to user handler
            self.send("DEBUG", "Received unrecognized request type", {"request": request_data})

        except Exception as e:
            self.send("ERROR", f"Error handling request: {e}")

    # ── Main loop ───────────────────────────────────────────────────

    def run(self):
        """Public entry point — call this to start the worker loop."""
        if not self.__ok:
            print("Worker initialization failed, cannot start")
            return
        self.__run()

    def __run(self):
        print(f"Worker listening on {self.__sock_path}")

        while self.__ok:
            try:
                request = self.__recv_message()
                if not request:
                    continue

                # Internal dispatch (EXECUTE, TERMINATE, META, RETRY)
                self.__handle_request(request)

                # User-defined handler — only for non-internal message types
                if request.get("type") not in self._INTERNAL_TYPES:
                    self.handle_request(request)

            except Exception as e:
                print(f"Error handling request: {e}")
                self.send("ERROR", str(e))
                self.__ok = False
                sys.exit(1)


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
