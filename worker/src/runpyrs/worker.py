"""Managed worker connection, protocol validation, and dispatch lifecycle."""

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
    """Base class for a Python process managed by Runpy.

    The Rust Manager supplies the Unix socket path, trusted worker ID, and
    optional ``--key=value`` arguments. Subclasses normally override
    :meth:`execute` for managed requests and :meth:`handle_envelope` for custom
    messages.

    Attributes:
        name: Trusted Manager-generated worker identity.
        extra: Parsed application ``--key=value`` arguments.
        stream: Connected Unix socket owned by the protocol lifecycle.

    Example:
        .. code-block:: python

            from runpyrs import RunScript, Worker

            class Parser(Worker):
                def execute(self, data: dict) -> dict:
                    return {"parsed": data["text"]}

            if __name__ == "__main__":
                RunScript(Parser)
    """

    _INTERNAL_OPS = _MANAGER_OPERATIONS

    def __init__(
        self,
        sock: str,
        name: str,
        extra: Optional[Dict[str, str]] = None,
    ):
        """Connect to the Manager and send the initial ``ready`` envelope.

        Args:
            sock: Manager-created Unix socket path.
            name: Exact Manager-generated worker identity.
            extra: Parsed ``--key=value`` arguments.

        Raises:
            ValueError: If ``name`` is empty.
            OSError: If socket creation, connection, or the initial send fails.
        """
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
        """Write one 8-byte little-endian length-prefixed payload.

        Any write failure marks the receive loop as closed and is re-raised.
        """
        try:
            self.stream.sendall(struct.pack("<Q", len(payload)) + payload)
        except Exception:
            self.__ok = False
            raise

    def _send_envelope(self, envelope: Envelope) -> None:
        """Copy, trust-stamp, serialize, and send one complete envelope.

        Caller-provided ``x_wid`` and ``x_spath`` values are overwritten with
        this connection's Manager-supplied identity and socket path.
        """
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
        """Build and send one Python-to-Manager operation envelope."""
        envelope = create_envelope(data, meta)
        envelope["meta"]["x_op"] = operation
        self._send_envelope(envelope)

    def send(self, data: Data, *, meta: Optional[Meta] = None) -> None:
        """Send an application-defined envelope without ``x_op``.

        Args:
            data: Application-owned dictionary body.
            meta: Optional application metadata without ``x_`` keys.

        Raises:
            TypeError: If ``data`` or ``meta`` is not a dictionary.
            ValueError: If metadata uses Runpy's reserved ``x_`` namespace.
            OSError: If the socket write fails.

        Example:
            ``self.send({"accepted": True}, meta={"request_id": 7})``
        """

        self._send_envelope(create_envelope(data, meta))

    def log(
        self,
        data: Data,
        *,
        level: str = "info",
        meta: Optional[Meta] = None,
    ) -> None:
        """Send a structured ``log`` operation.

        ``level`` always replaces a same-named metadata value. If the socket
        operation raises ``OSError``, one flushed stdout fallback is attempted:
        ``[runpy-log-fallback][level=<level>] <data repr>``. Fallback
        ``OSError`` and closed-stdout ``ValueError`` are suppressed, then the
        original transport error is re-raised. Serialization and metadata
        errors do not use the fallback.

        Args:
            data: Structured log dictionary.
            level: Developer-selected severity string.
            meta: Optional non-reserved metadata.

        Example:
            ``self.log({"message": "started"}, level="info")``
        """

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
        """Handle one custom Manager envelope without ``meta.x_op``.

        Override this hook for application-defined messages. An exception is
        converted to an ``error`` envelope by the receive loop.
        """

    def execute(self, data: ExecutePayload) -> ExecuteResult:
        """Process one ``execute`` or ``retry`` payload.

        Override this hook and return a dictionary for ``done.data`` or
        ``None`` for an empty result. Exceptions and non-dictionary results are
        converted to ``error`` envelopes; the receive loop remains active.
        """

        return None

    @staticmethod
    def _recv_exact(
        stream: socket.socket,
        size: int,
        *,
        allow_clean_close: bool = False,
    ) -> Optional[bytes]:
        """Read exactly ``size`` bytes from a socket.

        A clean close before any bytes returns ``None`` only when
        ``allow_clean_close`` is true. Mid-frame closure raises
        ``ConnectionError``.
        """
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
        """Read, decode, and validate one Manager-to-worker frame.

        The frame uses an 8-byte little-endian payload size followed by UTF-8
        JSON. Invalid encoding, JSON, shape, metadata, or operation direction
        raises ``ValueError``.
        """
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
        """Validate the exact envelope shape and Manager operation boundary.

        Custom envelopes omit ``x_op``. Only ``execute``, ``retry``, and
        ``terminate`` are valid internal operations from Manager to worker.
        """
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
        """Run the current execute payload and emit ``done`` or ``error``.

        Retry reuses the last payload. A retry before execute and an invalid
        result type produce explicit ``error`` messages.
        """
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
        """Dispatch one validated envelope to lifecycle or developer hooks."""
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
        """Receive and dispatch envelopes until termination or protocol failure.

        ``terminate`` closes normally. A malformed frame or handler-level
        protocol failure prints one diagnostic, marks the worker closed, and
        closes the socket.
        """

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
