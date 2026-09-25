import json
import socket
import struct
from typing import Dict, Optional

from .utils import (
    Data,
    Envelope,
    ExecutePayload,
    ExecuteResult,
    Meta,
    create_envelope,
)


_RUNPY_OPERATIONS = frozenset(
    {"ready", "execute", "retry", "terminate", "done", "error", "log"}
)
_MANAGER_OPERATIONS = frozenset({"execute", "retry", "terminate"})
_RESERVED_KEYS = frozenset({"x_wid", "x_spath", "x_op"})


class Worker:
    """Base class for Python workers managed by Runpy.

    Applications own the schema of both ``data`` and non-``x_`` metadata.
    Override ``execute`` for managed execution and ``handle_envelope`` for
    application-defined envelopes that do not contain ``meta.x_op``.
    """

    _INTERNAL_OPS = _MANAGER_OPERATIONS

    def __init__(
        self,
        sock: str,
        name: str,
        extra: Optional[Dict[str, str]] = None,
    ):
        if not name:
            raise ValueError("Worker ID is required")

        self.__ok = False
        self.__cycles = 0
        self.__exec_payload: Optional[Data] = None
        self.__sock_path = sock
        self.name = name
        self.extra: Dict[str, str] = extra or {}

        self.stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            self.stream.connect(self.__sock_path)
        except Exception:
            self.stream.close()
            raise

        self.__ok = True
        self._send_operation("ready", {})

    def _send_bytes(self, payload: bytes) -> None:
        try:
            self.stream.sendall(struct.pack("<Q", len(payload)) + payload)
        except Exception:
            self.__ok = False
            raise

    def _send_envelope(self, envelope: Envelope) -> None:
        meta = dict(envelope["meta"])
        meta["x_wid"] = self.name
        meta["x_spath"] = self.__sock_path
        stamped: Envelope = {"meta": meta, "data": dict(envelope["data"])}
        self._send_bytes(json.dumps(stamped, separators=(",", ":")).encode("utf-8"))

    def _send_operation(
        self,
        operation: str,
        data: Data,
        *,
        meta: Optional[Meta] = None,
    ) -> None:
        envelope = create_envelope(data, meta)
        envelope["meta"]["x_op"] = operation
        self._send_envelope(envelope)

    def send(self, data: Data, *, meta: Optional[Meta] = None) -> None:
        """Send an application-defined envelope to the Rust manager."""

        self._send_envelope(create_envelope(data, meta))

    def log(
        self,
        data: Data,
        *,
        level: str = "info",
        meta: Optional[Meta] = None,
    ) -> None:
        """Send a structured Runpy log envelope."""

        log_meta = dict(meta or {})
        log_meta["level"] = level
        try:
            self._send_operation("log", data, meta=log_meta)
        except OSError:
            try:
                print(
                    f"[runpy-log-fallback][level={level}] {data!r}",
                    flush=True,
                )
            except (OSError, ValueError):
                pass
            raise

    def handle_envelope(self, envelope: Envelope) -> None:
        """Handle an application-defined envelope."""

    def execute(self, data: ExecutePayload) -> ExecuteResult:
        """Run the worker's managed business logic."""

        return None

    @staticmethod
    def _recv_exact(
        stream: socket.socket,
        size: int,
        *,
        allow_clean_close: bool = False,
    ) -> Optional[bytes]:
        chunks = bytearray()
        while len(chunks) < size:
            chunk = stream.recv(size - len(chunks))
            if not chunk:
                if allow_clean_close and not chunks:
                    return None
                raise ConnectionError("Connection closed during a frame")
            chunks.extend(chunk)
        return bytes(chunks)

    def _recv_envelope(self) -> Optional[Envelope]:
        size_data = self._recv_exact(self.stream, 8, allow_clean_close=True)
        if size_data is None:
            return None

        size = struct.unpack("<Q", size_data)[0]
        payload = self._recv_exact(self.stream, size)
        if payload is None:
            raise ConnectionError("Connection closed before envelope payload")

        try:
            decoded = json.loads(payload.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise ValueError(f"Invalid JSON envelope: {error}") from error

        return self._validate_inbound(decoded)

    @staticmethod
    def _validate_inbound(value: object) -> Envelope:
        if not isinstance(value, dict) or set(value) != {"meta", "data"}:
            raise ValueError("Envelope must contain exactly 'meta' and 'data'")

        meta = value["meta"]
        data = value["data"]
        if not isinstance(meta, dict) or not isinstance(data, dict):
            raise ValueError("Envelope 'meta' and 'data' must be objects")

        for key, item in meta.items():
            if not key.startswith("x_"):
                continue
            if key not in _RESERVED_KEYS:
                raise ValueError(f"Unknown Runpy metadata key '{key}'")
            if not isinstance(item, str):
                raise ValueError(f"Runpy metadata '{key}' must be a string")

        operation = meta.get("x_op")
        if operation is not None:
            if operation not in _RUNPY_OPERATIONS:
                raise ValueError(f"Unknown Runpy operation '{operation}'")
            if operation not in _MANAGER_OPERATIONS:
                raise ValueError(
                    f"Runpy operation '{operation}' is invalid for manager-to-worker envelopes"
                )

        return {"meta": dict(meta), "data": dict(data)}

    def _complete_execution(self, *, retry: bool = False) -> None:
        if self.__exec_payload is None:
            self._send_operation(
                "error", {"message": "Retry requested before an execute operation"}
            )
            return

        try:
            result = self.execute(self.__exec_payload)
            if result is not None and not isinstance(result, dict):
                self._send_operation(
                    "error",
                    {"message": "Worker.execute must return a dictionary or None"},
                )
                return
            self._send_operation("done", result or {})
        except Exception as error:
            prefix = "Retry execution error" if retry else "Execution error"
            self._send_operation("error", {"message": f"{prefix}: {error}"})

    def _dispatch(self, envelope: Envelope) -> None:
        operation = envelope["meta"].get("x_op")
        if operation == "terminate":
            self.__ok = False
            self.stream.close()
            return

        if operation == "execute":
            self.__exec_payload = envelope["data"]
            self._complete_execution()
            return

        if operation == "retry":
            self.__cycles += 1
            self._complete_execution(retry=True)
            return

        try:
            self.handle_envelope(envelope)
        except Exception as error:
            self._send_operation(
                "error", {"message": f"Envelope handler error: {error}"}
            )

    def run(self) -> None:
        """Receive and dispatch envelopes until the connection closes."""

        while self.__ok:
            try:
                envelope = self._recv_envelope()
                if envelope is None:
                    self.__ok = False
                    self.stream.close()
                    break
                self._dispatch(envelope)
            except Exception as error:
                print(f"Protocol violation: {error}")
                self.__ok = False
                self.stream.close()
                break
