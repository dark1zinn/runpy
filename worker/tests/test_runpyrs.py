import json
import socket
import struct
import sys
import tempfile
import threading
from typing import get_args, get_origin, get_type_hints, Union

import pytest

import runpyrs
from runpyrs import (
    Data,
    Envelope,
    ExecutePayload,
    ExecuteResult,
    Meta,
    RunScript,
    RunpyOperation,
    Worker,
    create_envelope,
)


def _read_exact(stream: socket.socket, size: int) -> bytes:
    payload = bytearray()
    while len(payload) < size:
        chunk = stream.recv(size - len(payload))
        if not chunk:
            raise ConnectionError("connection closed")
        payload.extend(chunk)
    return bytes(payload)


def read_envelope(stream: socket.socket) -> dict:
    size = struct.unpack("<Q", _read_exact(stream, 8))[0]
    return json.loads(_read_exact(stream, size).decode("utf-8"))


def send_envelope(stream: socket.socket, envelope: object) -> None:
    payload = json.dumps(envelope, separators=(",", ":")).encode("utf-8")
    stream.sendall(struct.pack("<Q", len(payload)) + payload)


class EchoWorker(Worker):
    def execute(self, data: dict) -> dict:
        return {"echo": data}


def open_worker(worker_type: type[Worker] = EchoWorker):
    temp = tempfile.TemporaryDirectory()
    socket_path = f"{temp.name}/worker.sock"
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(socket_path)
    listener.listen(1)
    worker = worker_type(socket_path, "worker-123")
    connection, _ = listener.accept()
    connection.settimeout(1)
    ready = read_envelope(connection)
    return temp, listener, connection, worker, ready, socket_path


def close_worker(resources) -> None:
    temp, listener, connection, worker, _, _ = resources
    try:
        connection.close()
    finally:
        listener.close()
        worker.stream.close()
        temp.cleanup()


def test_public_exports_are_the_bare_envelope_api():
    assert runpyrs.__all__ == [
        "Worker",
        "RunScript",
        "Envelope",
        "Meta",
        "Data",
        "RunpyOperation",
        "create_envelope",
        "ExecutePayload",
        "ExecuteResult",
    ]
    assert get_type_hints(Envelope) == {"meta": Meta, "data": Data}
    assert ExecutePayload is Data
    assert get_origin(ExecuteResult) is Union
    assert type(None) in get_args(ExecuteResult)
    assert RunpyOperation is not None

    for removed in (
        "Message",
        "Method",
        "Headers",
        "OutboundMessage",
        "InternalMessageType",
        "BuiltinResponseType",
        "MessageType",
    ):
        assert not hasattr(runpyrs, removed)


def test_create_envelope_preserves_json_metadata_and_copies_inputs():
    meta = {"some_custom_meta": 42}
    data = {"some": "data"}
    envelope = create_envelope(data, meta)
    meta["some_custom_meta"] = 0
    data["some"] = "changed"

    assert envelope == {
        "meta": {"some_custom_meta": 42},
        "data": {"some": "data"},
    }


@pytest.mark.parametrize("key", ["x_wid", "x_spath", "x_op", "x_custom"])
def test_create_envelope_rejects_reserved_metadata(key):
    with pytest.raises(ValueError, match="reserved by Runpy"):
        create_envelope({}, {key: "spoofed"})


@pytest.mark.parametrize("field", ["meta", "data"])
def test_create_envelope_requires_objects(field):
    kwargs = {"data": {}}
    if field == "data":
        kwargs["data"] = []
    else:
        kwargs["meta"] = []

    with pytest.raises(TypeError, match=f"{field} must be a dictionary"):
        create_envelope(**kwargs)


def test_worker_ready_envelope_uses_exact_argv_identity_and_socket():
    resources = open_worker()
    try:
        _, _, _, _, ready, socket_path = resources
        assert ready == {
            "meta": {
                "x_op": "ready",
                "x_wid": "worker-123",
                "x_spath": socket_path,
            },
            "data": {},
        }
    finally:
        close_worker(resources)


def test_custom_send_preserves_numeric_metadata():
    resources = open_worker()
    try:
        _, _, connection, worker, _, socket_path = resources
        worker.send({"some": "data"}, meta={"some_custom_meta": 42})
        assert read_envelope(connection) == {
            "meta": {
                "some_custom_meta": 42,
                "x_wid": "worker-123",
                "x_spath": socket_path,
            },
            "data": {"some": "data"},
        }
    finally:
        close_worker(resources)


def test_successful_log_does_not_write_stdout_fallback(capsys):
    resources = open_worker()
    try:
        _, _, connection, worker, _, _ = resources
        worker.log(
            {"message": "hello"},
            level="warning",
            meta={"level": "debug", "correlation_id": 7},
        )
        envelope = read_envelope(connection)
        assert envelope["meta"]["x_op"] == "log"
        assert envelope["meta"]["level"] == "warning"
        assert envelope["meta"]["correlation_id"] == 7
        assert envelope["data"] == {"message": "hello"}
        assert capsys.readouterr().out == ""
    finally:
        close_worker(resources)


def test_log_transport_failure_prints_flushed_fallback_and_reraises(monkeypatch):
    resources = open_worker()
    try:
        _, _, _, worker, _, _ = resources
        failure = BrokenPipeError("socket closed")
        printed = []

        def fail_send(*args, **kwargs):
            raise failure

        def record_print(*args, **kwargs):
            printed.append((args, kwargs))

        monkeypatch.setattr(worker, "_send_operation", fail_send)
        monkeypatch.setattr("builtins.print", record_print)
        with pytest.raises(BrokenPipeError) as raised:
            worker.log({"message": "lost"}, level="warning")

        assert raised.value is failure
        assert printed == [
            (
                ("[runpy-log-fallback][level=warning] {'message': 'lost'}",),
                {"flush": True},
            )
        ]
    finally:
        close_worker(resources)


def test_log_fallback_failure_preserves_original_transport_error(monkeypatch):
    resources = open_worker()
    try:
        _, _, _, worker, _, _ = resources
        failure = BrokenPipeError("socket closed")

        def fail_send(*args, **kwargs):
            raise failure

        def fail_print(*args, **kwargs):
            raise OSError("stdout closed")

        monkeypatch.setattr(worker, "_send_operation", fail_send)
        monkeypatch.setattr("builtins.print", fail_print)
        with pytest.raises(BrokenPipeError) as raised:
            worker.log({"message": "lost"})

        assert raised.value is failure
    finally:
        close_worker(resources)


def test_log_serialization_error_does_not_print_fallback(capsys):
    resources = open_worker()
    try:
        _, _, _, worker, _, _ = resources
        with pytest.raises(TypeError):
            worker.log({"invalid": object()})
        assert capsys.readouterr().out == ""
    finally:
        close_worker(resources)


def test_execute_returns_direct_done_data():
    resources = open_worker()
    try:
        _, _, connection, worker, _, socket_path = resources
        thread = threading.Thread(target=worker.run)
        thread.start()

        send_envelope(
            connection,
            {
                "meta": {
                    "x_op": "execute",
                    "x_wid": "worker-123",
                    "x_spath": socket_path,
                },
                "data": {"value": 9},
            },
        )
        done = read_envelope(connection)
        assert done["meta"]["x_op"] == "done"
        assert done["data"] == {"echo": {"value": 9}}

        send_envelope(
            connection,
            {"meta": {"x_op": "terminate"}, "data": {}},
        )
        thread.join(timeout=1)
        assert not thread.is_alive()
    finally:
        close_worker(resources)


def test_custom_inbound_envelope_reaches_developer_hook_once():
    received = []
    called = threading.Event()

    class CustomWorker(Worker):
        def handle_envelope(self, envelope: Envelope) -> None:
            received.append(envelope)
            called.set()

    resources = open_worker(CustomWorker)
    try:
        _, _, connection, worker, _, _ = resources
        thread = threading.Thread(target=worker.run)
        thread.start()
        custom = {
            "meta": {
                "x_wid": "worker-123",
                "x_spath": "/tmp/manager.sock",
                "some_custom_meta": 42,
            },
            "data": {"some": "data"},
        }
        send_envelope(connection, custom)
        assert called.wait(timeout=1)
        assert received == [custom]

        send_envelope(
            connection,
            {"meta": {"x_op": "terminate"}, "data": {}},
        )
        thread.join(timeout=1)
    finally:
        close_worker(resources)


def test_retry_before_execute_returns_error():
    resources = open_worker()
    try:
        _, _, connection, worker, _, _ = resources
        thread = threading.Thread(target=worker.run)
        thread.start()
        send_envelope(connection, {"meta": {"x_op": "retry"}, "data": {}})
        error = read_envelope(connection)
        assert error["meta"]["x_op"] == "error"
        assert error["data"] == {
            "message": "Retry requested before an execute operation"
        }
        send_envelope(
            connection,
            {"meta": {"x_op": "terminate"}, "data": {}},
        )
        thread.join(timeout=1)
    finally:
        close_worker(resources)


def test_invalid_execute_result_returns_error():
    class InvalidResultWorker(Worker):
        def execute(self, data):
            return ["not", "an", "object"]

    resources = open_worker(InvalidResultWorker)
    try:
        _, _, connection, worker, _, _ = resources
        thread = threading.Thread(target=worker.run)
        thread.start()
        send_envelope(
            connection,
            {"meta": {"x_op": "execute"}, "data": {}},
        )
        error = read_envelope(connection)
        assert error["meta"]["x_op"] == "error"
        assert error["data"] == {
            "message": "Worker.execute must return a dictionary or None"
        }
        send_envelope(
            connection,
            {"meta": {"x_op": "terminate"}, "data": {}},
        )
        thread.join(timeout=1)
    finally:
        close_worker(resources)


@pytest.mark.parametrize(
    "invalid",
    [
        {"meta": {}, "data": []},
        {"meta": {"x_custom": "no"}, "data": {}},
        {"meta": {"x_op": 42}, "data": {}},
        {"meta": {"x_op": "ready"}, "data": {}},
    ],
)
def test_protocol_violation_closes_without_response(invalid):
    resources = open_worker()
    try:
        _, _, connection, worker, _, _ = resources
        thread = threading.Thread(target=worker.run)
        thread.start()
        send_envelope(connection, invalid)
        thread.join(timeout=1)
        assert not thread.is_alive()
        assert connection.recv(1) == b""
    finally:
        close_worker(resources)


def test_runscript_requires_socket_and_worker_id(monkeypatch):
    monkeypatch.setattr(sys, "argv", ["test"])
    with pytest.raises(SystemExit):
        RunScript(Worker)

    monkeypatch.setattr(sys, "argv", ["test", "/tmp/fake.sock"])
    with pytest.raises(SystemExit):
        RunScript(Worker)


def test_runscript_checks_subclass_with_complete_arguments(monkeypatch):
    monkeypatch.setattr(sys, "argv", ["test", "/tmp/fake.sock", "worker-123"])
    with pytest.raises(SystemExit):
        RunScript(object)


def test_runscript_passes_exact_worker_id(monkeypatch):
    captured = {}

    class CapturingWorker(Worker):
        def __init__(self, sock, name, extra):
            captured.update(sock=sock, name=name, extra=extra)

        def run(self):
            captured["ran"] = True

    monkeypatch.setattr(
        sys,
        "argv",
        ["test", "/tmp/worker.sock", "worker-123", "--mode=fast"],
    )
    RunScript(CapturingWorker)
    assert captured == {
        "sock": "/tmp/worker.sock",
        "name": "worker-123",
        "extra": {"mode": "fast"},
        "ran": True,
    }
