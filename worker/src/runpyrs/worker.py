import socket
import json
import sys
import struct
from typing import Optional, Dict, Any

from .utils import (
    ExecutePayload,
    ExecuteResult,
    HandleRequestResult,
    Headers,
    HeadersDict,
    Message,
    Method,
    RequestData,
    create_message,
    ready_message,
    done_message,
    error_message,
    debug_message,
)


class Worker:
    """Base class for Python workers managed by Runpy.

    Subclass this and override `execute()` (required) and optionally
    `handle_request()` for custom message routing.

    Messages follow an HTTP-like schema:
        {
            "method": "EXECUTE",
            "path": "/execute",
            "headers": {
                "X-Worker-Id": "worker_name",
                "X-Socket-Path": "/tmp/runpy/rp_xxx.sock"
            },
            "body": { ... }
        }

    Usage::

        class MyWorker(Worker):
            def execute(self, payload: dict) -> dict:
                return {"result": "ok"}

        if __name__ == "__main__":
            RunScript(MyWorker)
    """

    def __init__(self, sock: str):
        self.__ok = False
        self.__cycles = 0
        self.__exec_payload: Optional[Dict[str, Any]] = None
        self.__sock_path = sock
        self.name: Optional[str] = None

        try:
            self.stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            self.stream.connect(self.__sock_path)
        except Exception as e:
            print(f"Failed to set up socket: {e}")
            sys.exit(1)

        self.__ok = True
        self.send("READY", "/ready", message="Worker is ready to receive requests")

    # ══════════════════════════════════════════════════════════════════════════
    # HEADERS
    # ══════════════════════════════════════════════════════════════════════════

    def _build_headers(self, extra: Optional[HeadersDict] = None) -> HeadersDict:
        """Build headers dict with worker identification.

        All outbound messages include:
            - X-Worker-Id: Worker's name/identifier
            - X-Socket-Path: Path to the Unix socket
            - Content-Type: application/json
        """
        headers: HeadersDict = {
            Headers.CONTENT_TYPE: "application/json",
            Headers.X_SOCKET_PATH: self.__sock_path,
        }
        if self.name:
            headers[Headers.X_WORKER_ID] = self.name
        if extra:
            headers.update(extra)
        return headers

    # ══════════════════════════════════════════════════════════════════════════
    # MESSAGING
    # ══════════════════════════════════════════════════════════════════════════

    def __send_message(self, data: bytes):
        """Send a length-prefixed message."""
        try:
            size = struct.pack("<Q", len(data))
            self.stream.sendall(size + data)
        except Exception as e:
            print(f"Failed to send message: {e}")
            self.__ok = False

    def __recv_message(self) -> Optional[Message]:
        """Receive a length-prefixed message."""
        try:
            size_data = self.stream.recv(8)
            if not size_data:
                return None

            size = struct.unpack("<Q", size_data)[0]

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

    def send(
        self,
        method: Method,
        path: str = "",
        *,
        message: Optional[str] = None,
        body: Optional[Dict[str, Any]] = None,
        headers: Optional[HeadersDict] = None,
    ):
        """Send a typed message back to Rust's Control Plane.

        Args:
            method: The HTTP-like method (READY, DONE, ERROR, DEBUG, INFO, etc.)
            path: Resource path (e.g., "/ready", "/done", "/error")
            message: Optional message string (added to body)
            body: Optional body dict (merged with message if provided)
            headers: Optional extra headers (merged with default worker headers)
        """
        try:
            if not method:
                raise ValueError("Method is required")

            # Build the body
            msg_body: Dict[str, Any] = body.copy() if body else {}
            if message is not None:
                msg_body["message"] = message

            # Build headers with worker identification
            msg_headers = self._build_headers(headers)

            # Create the HTTP-like message
            payload = create_message(
                method=method.upper(),
                path=path or f"/{method.lower()}",
                headers=msg_headers,
                body=msg_body if msg_body else None,
            )

            self.__send_message(json.dumps(payload).encode())
        except Exception as e:
            print(f"Failed to notify Rust: {e}")
            self.__ok = False
            sys.exit(1)

    # ══════════════════════════════════════════════════════════════════════════
    # OVERRIDABLE HOOKS
    # ══════════════════════════════════════════════════════════════════════════

    def handle_request(self, request_data: RequestData) -> HandleRequestResult:
        """Handle incoming requests from Rust.

        Override this method in your worker subclass to implement custom
        message routing for types that are **not** handled internally
        (EXECUTE, TERMINATE, META, RETRY).

        The default implementation is a no-op so that subclasses are not
        forced to override it.
        """
        pass

    def execute(self, payload: ExecutePayload) -> ExecuteResult:
        """Main business logic.

        Called when an EXECUTE message is received. Override this in your
        subclass and return a dict with the result. The result is sent
        back to Rust as a DONE message.
        """
        return None

    # ══════════════════════════════════════════════════════════════════════════
    # INTERNAL REQUEST HANDLING
    # ══════════════════════════════════════════════════════════════════════════

    # Methods handled internally — custom `handle_request` will NOT be called for these.
    _INTERNAL_METHODS = frozenset({"TERMINATE", "META", "EXECUTE", "RETRY"})

    def __handle_request(self, request_data: Message):
        """Internal dispatcher for protocol-level messages."""
        try:
            method = request_data.get("method")
            body = request_data.get("body", {})
            headers = request_data.get("headers", {})

            if method is None:
                self.send("DEBUG", "/debug", message="Received request with no method", body={"request": request_data})
                return

            if method == "TERMINATE":
                self.stream.close()
                sys.exit(0)

            if method == "META":
                # Extract worker name from body if provided
                if body.get("name"):
                    self.name = body["name"]
                self.send("DEBUG", "/debug", message="Received META data", body=body)
                return

            if method == "EXECUTE":
                self.__exec_payload = body
                self.send("DEBUG", "/debug", message="Received EXECUTE request", body={"payload": self.__exec_payload})
                try:
                    result = self.execute(self.__exec_payload)
                    if result is not None:
                        self.send("DONE", "/done", message="Execution completed", body={"data": result})
                    else:
                        self.send("DONE", "/done", message="Execution completed with no result", body={})
                except Exception as e:
                    self.send("ERROR", "/error", message=f"Execution error: {e}")
                return

            if method == "RETRY":
                self.__cycles += 1
                self.send(
                    "DEBUG",
                    "/debug",
                    message=f"Received RETRY request, re-executing (cycle: {self.__cycles})",
                    body={},
                )
                try:
                    result = self.execute(self.__exec_payload)
                    if result is not None:
                        self.send(
                            "DONE",
                            "/done",
                            message=f"Execution completed on retry({self.__cycles})",
                            body={"data": result},
                        )
                    else:
                        self.send(
                            "DONE",
                            "/done",
                            message=f"Execution completed on retry({self.__cycles}) with no result",
                            body={},
                        )
                except Exception as e:
                    self.send("ERROR", "/error", message=f"Execution error on retry: {e}")
                return

            # Not an internal method — fall through to user handler
            self.send("DEBUG", "/debug", message="Received unrecognized method", body={"request": request_data})

        except Exception as e:
            self.send("ERROR", "/error", message=f"Error handling request: {e}")

    # ══════════════════════════════════════════════════════════════════════════
    # MAIN LOOP
    # ══════════════════════════════════════════════════════════════════════════

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

                # User-defined handler — only for non-internal methods
                if request.get("method") not in self._INTERNAL_METHODS:
                    self.handle_request(request)

            except Exception as e:
                print(f"Error handling request: {e}")
                self.send("ERROR", "/error", message=str(e))
                self.__ok = False
                sys.exit(1)
