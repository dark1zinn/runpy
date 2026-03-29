"""Basic sanity checks for the runpyrs package."""

import sys
import pytest

from runpyrs import Worker, RunScript
from runpyrs.worker import Worker as WorkerDirect
from runpyrs.runScript import RunScript as RunScriptDirect

# Typing utilities
from runpyrs import (
    Envelope,
    ExecutePayload,
    ExecuteResult,
    HandleRequestResult,
    InternalMessageType,
    BuiltinResponseType,
    MessageType,
    MetaData,
    OutboundMessage,
    RequestData,
    SendData,
)
from runpyrs.utils import (
    Envelope as EnvelopeDirect,
    ExecuteResult as ExecuteResultDirect,
)


# ── Package imports ─────────────────────────────────────────────────────


class TestPackageImports:
    """Verify that public symbols are importable and consistent."""

    def test_worker_importable(self):
        assert Worker is not None

    def test_runscript_importable(self):
        assert RunScript is not None

    def test_top_level_reexports_match_modules(self):
        assert Worker is WorkerDirect
        assert RunScript is RunScriptDirect

    def test_all_exports(self):
        import runpyrs

        assert hasattr(runpyrs, "__all__")
        assert "Worker" in runpyrs.__all__
        assert "RunScript" in runpyrs.__all__
        # Typing utilities must also be exported
        for name in (
            "Envelope",
            "ExecutePayload",
            "ExecuteResult",
            "HandleRequestResult",
            "InternalMessageType",
            "BuiltinResponseType",
            "MessageType",
            "MetaData",
            "OutboundMessage",
            "RequestData",
            "SendData",
        ):
            assert name in runpyrs.__all__, f"{name} missing from __all__"


# ── Worker class ────────────────────────────────────────────────────────


class TestWorkerClass:
    """Check Worker interface without needing a live socket."""

    def test_worker_is_a_class(self):
        assert isinstance(Worker, type)

    def test_execute_is_overridable(self):
        class Custom(Worker):
            def __init__(self):
                # Skip real socket setup
                pass

            def execute(self, payload: dict) -> dict:
                return {"echo": payload}

        w = Custom()
        assert w.execute({"x": 1}) == {"echo": {"x": 1}}

    def test_handle_request_default_is_noop(self):
        class Noop(Worker):
            def __init__(self):
                pass

        w = Noop()
        assert w.handle_request({}) is None

    def test_send_method_exists(self):
        assert callable(getattr(Worker, "send", None))

    def test_run_method_exists(self):
        assert callable(getattr(Worker, "run", None))

    def test_internal_types_defined(self):
        expected = {"TERMINATE", "META", "EXECUTE", "RETRY"}
        assert Worker._INTERNAL_TYPES == expected


# ── RunScript ───────────────────────────────────────────────────────────


class TestRunScript:
    """RunScript helper checks (no live socket)."""

    def test_runscript_is_callable(self):
        assert callable(RunScript)

    def test_runscript_rejects_non_worker(self):
        """RunScript should fail when given a class that is not a Worker subclass."""
        with pytest.raises(SystemExit):
            # Provide a fake argv so it doesn't fail on missing arg first
            original_argv = sys.argv
            sys.argv = ["test", "/tmp/fake.sock"]
            try:
                RunScript(object)
            finally:
                sys.argv = original_argv

    def test_runscript_requires_socket_arg(self):
        """RunScript should exit when sys.argv has no socket path."""
        original_argv = sys.argv
        sys.argv = ["test"]
        try:
            with pytest.raises(SystemExit):
                RunScript(Worker)
        finally:
            sys.argv = original_argv


# ── Typing utilities ────────────────────────────────────────────────────


class TestTypingUtils:
    """Verify that type aliases and TypedDicts are usable at runtime."""

    def test_reexports_match_module(self):
        assert Envelope is EnvelopeDirect
        assert ExecuteResult is ExecuteResultDirect

    def test_envelope_is_typed_dict(self):
        # TypedDict classes have __annotations__ and descend from dict
        assert hasattr(Envelope, "__annotations__")
        assert "type" in Envelope.__annotations__
        assert "data" in Envelope.__annotations__

    def test_outbound_message_is_typed_dict(self):
        assert hasattr(OutboundMessage, "__annotations__")
        assert "type" in OutboundMessage.__annotations__
        assert "message" in OutboundMessage.__annotations__
        assert "data" in OutboundMessage.__annotations__

    def test_meta_data_is_typed_dict(self):
        assert hasattr(MetaData, "__annotations__")
        assert "name" in MetaData.__annotations__

    def test_execute_payload_is_dict_alias(self):
        # ExecutePayload is Dict[str, Any] — at runtime it's a generic alias
        assert ExecutePayload is not None

    def test_execute_result_allows_none(self):
        """ExecuteResult should accept both dict and None."""
        from typing import get_args, get_origin, Union

        # Optional[X] is Union[X, None]
        origin = get_origin(ExecuteResult)
        assert origin is Union
        args = get_args(ExecuteResult)
        assert type(None) in args

    def test_request_data_is_envelope_alias(self):
        assert RequestData is Envelope

    def test_worker_execute_uses_typed_aliases(self):
        """Worker.execute annotations should reference the new type aliases."""
        hints = Worker.execute.__annotations__
        assert "payload" in hints
        assert "return" in hints
